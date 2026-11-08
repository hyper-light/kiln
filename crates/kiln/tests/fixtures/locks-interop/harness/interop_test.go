package main

// W29-AC20/AC22 (kiln plan 01a19a37, step 12). kiln's harness/run.sh copies this file into a
// scratch copy of github/gh-actions-lock at 113f91d77e0af03838b6ae3bef0e038cbab4c696, beside
// cmd/gh-actions-lock's own tests, whose helpers it uses. A fake host answers every request the
// pinned CLI makes from a case's recorded facts, and a request no fact answers fails the case.
// For each case it writes the CLI's actions.lock, the official parser's reading of it, and what
// --verify finds in each scenario.

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"testing"

	parserlock "github.com/github/actions-lockfile/go/pkg/lockfile"
	"github.com/stretchr/testify/require"
)

type kilnRepo struct {
	OwnerID       int64                        `json:"owner_id"`
	RepoID        int64                        `json:"repo_id"`
	DefaultBranch string                       `json:"default_branch"`
	Refs          map[string]string            `json:"refs"`
	Commits       map[string][]string          `json:"commits"`
	Files         map[string]map[string]string `json:"files"`
}

type kilnFacts struct {
	Repositories map[string]*kilnRepo `json:"repositories"`
}

type kilnScenario struct {
	Name    string `json:"name"`
	Lock    string `json:"lock"`
	Changes []struct {
		Workflow *struct {
			Path string `json:"path"`
			From string `json:"from"`
			To   string `json:"to"`
		} `json:"workflow"`
		Move *struct {
			Repository string            `json:"repository"`
			Ref        string            `json:"ref"`
			To         string            `json:"to"`
			Parents    []string          `json:"parents"`
			Files      map[string]string `json:"files"`
		} `json:"move"`
		Identity *struct {
			Repository string `json:"repository"`
			OwnerID    int64  `json:"owner_id"`
			RepoID     int64  `json:"repo_id"`
		} `json:"identity"`
	} `json:"changes"`
}

// peel resolves an expression as the host's rev-parse does: a full commit id it has, else a
// tag, else a branch, of exactly that name.
func (r *kilnRepo) peel(expr string) string {
	expr = strings.TrimSuffix(expr, "^{commit}")
	if parserlock.IsFullSha(expr) {
		if _, ok := r.Commits[strings.ToLower(expr)]; ok {
			return strings.ToLower(expr)
		}
		return ""
	}
	for _, name := range []string{expr, "refs/tags/" + expr, "refs/heads/" + expr} {
		if c, ok := r.Refs[name]; ok && strings.HasPrefix(name, "refs/") {
			return c
		}
	}
	return ""
}

// ancestor reports whether commit a is in the history of commit b.
func (r *kilnRepo) ancestor(a, b string) bool {
	seen := map[string]bool{}
	for todo := []string{b}; len(todo) > 0; {
		c := todo[len(todo)-1]
		todo = todo[:len(todo)-1]
		if strings.EqualFold(c, a) {
			return true
		}
		if !seen[c] {
			seen[c] = true
			todo = append(todo, r.Commits[c]...)
		}
	}
	return false
}

// compare is GitHub's status of head against base.
func (r *kilnRepo) compare(base, head string) string {
	switch {
	case strings.EqualFold(base, head):
		return "identical"
	case r.ancestor(base, head):
		return "ahead"
	case r.ancestor(head, base):
		return "behind"
	}
	return "diverged"
}

func (r *kilnRepo) refs(prefix string) []map[string]any {
	names := []string{}
	for name := range r.Refs {
		if strings.HasPrefix(name, prefix) {
			names = append(names, name)
		}
	}
	sort.Strings(names)
	out := []map[string]any{}
	for _, name := range names {
		out = append(out, map[string]any{"name": strings.TrimPrefix(name, prefix), "commit": map[string]any{"sha": r.Refs[name]}, "protected": true, "ref": name, "object": map[string]any{"sha": r.Refs[name], "type": "commit"}})
	}
	return out
}

type kilnHost struct {
	facts      kilnFacts
	mu         sync.Mutex
	unanswered []string
}

// raw opts out of the package's parityFallback, which would call an unanswered pin intact.
func (*kilnHost) raw() {}

func (h *kilnHost) repo(owner, name string) (string, *kilnRepo) {
	for nwo, r := range h.facts.Repositories {
		if strings.EqualFold(nwo, owner+"/"+name) {
			return nwo, r
		}
	}
	return "", nil
}

func (h *kilnHost) RoundTrip(req *http.Request) (*http.Response, error) {
	var body []byte
	if req.Body != nil {
		body, _ = io.ReadAll(req.Body)
	}
	status, answer, known := h.answer(req, body)
	if !known {
		h.mu.Lock()
		h.unanswered = append(h.unanswered, req.Method+" "+req.URL.String()+" "+string(body))
		h.mu.Unlock()
		status, answer = http.StatusNotFound, map[string]any{"message": "kiln interop: no recorded fact answers this"}
	}
	payload, ok := answer.(string)
	if !ok {
		encoded, err := json.Marshal(answer)
		if err != nil {
			return nil, err
		}
		payload = string(encoded)
	}
	return &http.Response{StatusCode: status, Status: fmt.Sprintf("%d %s", status, http.StatusText(status)), Header: http.Header{"Content-Type": []string{"application/json"}}, Body: io.NopCloser(strings.NewReader(payload)), Request: req}, nil
}

func (h *kilnHost) done(t *testing.T) {
	t.Helper()
	h.mu.Lock()
	defer h.mu.Unlock()
	for _, r := range h.unanswered {
		t.Errorf("no recorded fact answers %s", r)
	}
}

var kilnNotFound = map[string]any{"message": "Not Found"}

func (h *kilnHost) answer(req *http.Request, body []byte) (int, any, bool) {
	path := strings.TrimPrefix(req.URL.Path, "/api/v3")
	if req.Method == http.MethodPost && (path == "/graphql" || path == "/api/graphql") {
		return h.graphql(body)
	}
	parts := strings.Split(strings.TrimPrefix(path, "/"), "/")
	if req.Method != http.MethodGet || len(parts) < 3 || parts[0] != "repos" {
		return 0, nil, false
	}
	nwo, repo := h.repo(parts[1], parts[2])
	if repo == nil {
		return http.StatusNotFound, kilnNotFound, true
	}
	rest := strings.Join(parts[3:], "/")
	switch {
	case rest == "":
		owner, name, _ := strings.Cut(nwo, "/")
		return http.StatusOK, map[string]any{"full_name": nwo, "name": name, "default_branch": repo.DefaultBranch, "id": repo.RepoID, "owner": map[string]any{"id": repo.OwnerID, "login": owner}, "private": false, "visibility": "public"}, true
	case rest == "tags":
		return http.StatusOK, repo.refs("refs/tags/"), true
	case rest == "branches":
		return http.StatusOK, repo.refs("refs/heads/"), true
	case rest == "releases":
		return http.StatusOK, []any{}, true
	case strings.HasPrefix(rest, "git/ref/"):
		qualified := "refs/" + strings.TrimPrefix(rest, "git/ref/")
		if c, ok := repo.Refs[qualified]; ok {
			return http.StatusOK, map[string]any{"ref": qualified, "object": map[string]any{"sha": c, "type": "commit"}}, true
		}
		return http.StatusNotFound, kilnNotFound, true
	case strings.HasPrefix(rest, "git/matching-refs/"):
		return http.StatusOK, repo.refs("refs/" + strings.TrimPrefix(rest, "git/matching-refs/")), true
	case strings.HasPrefix(rest, "commits/") && !strings.Contains(strings.TrimPrefix(rest, "commits/"), "/"):
		if c := repo.peel(strings.TrimPrefix(rest, "commits/")); c != "" {
			return http.StatusOK, map[string]any{"sha": c}, true
		}
		return http.StatusNotFound, kilnNotFound, true
	case strings.HasPrefix(rest, "compare/"):
		base, head, ok := strings.Cut(strings.TrimPrefix(rest, "compare/"), "...")
		b, c := repo.peel(base), repo.peel(head)
		if !ok || b == "" || c == "" {
			return http.StatusNotFound, kilnNotFound, true
		}
		status := repo.compare(b, c)
		merge := map[string]any{"ahead": b, "behind": c, "identical": b}[status]
		return http.StatusOK, map[string]any{"status": status, "merge_base_commit": map[string]any{"sha": merge}}, true
	case strings.HasPrefix(rest, "contents/"):
		ref := req.URL.Query().Get("ref")
		if ref == "" {
			ref = repo.DefaultBranch
		}
		if text, ok := repo.Files[repo.peel(ref)][strings.TrimPrefix(rest, "contents/")]; ok {
			return http.StatusOK, text, true
		}
		return http.StatusNotFound, kilnNotFound, true
	}
	return 0, nil, false
}

func (h *kilnHost) graphql(body []byte) (int, any, bool) {
	var q struct {
		Query     string         `json:"query"`
		Variables map[string]any `json:"variables"`
	}
	if json.Unmarshal(body, &q) != nil {
		return 0, nil, false
	}
	str := func(key string, i int) (string, bool) {
		s, ok := q.Variables[fmt.Sprintf("%s%d", key, i)].(string)
		return s, ok
	}
	data := map[string]any{}
	errs := []any{}
	missing := func(alias string) {
		data[alias] = nil
		errs = append(errs, map[string]any{"type": "NOT_FOUND", "path": []string{alias}, "message": "Could not resolve to a Repository"})
	}
	_, files := q.Variables["expr0"]
	_, parity := q.Variables["oid0"]
	_, reach := q.Variables["sha"]
	switch {
	case files:
		for i := 0; ; i++ {
			owner, ok := str("owner", i)
			if !ok {
				break
			}
			name, _ := str("name", i)
			alias := fmt.Sprintf("a%d", i)
			nwo, repo := h.repo(owner, name)
			if repo == nil {
				missing(alias)
				continue
			}
			expr, _ := str("expr", i)
			c := repo.peel(expr)
			if c == "" {
				data[alias] = map[string]any{"nameWithOwner": nwo, "object": nil}
				continue
			}
			object := map[string]any{"oid": c}
			for field, key := range map[string]string{"file": "yml", "fileYaml": "yaml"} {
				object[field] = nil
				if path, _ := str(key, i); path != "" {
					if text, ok := repo.Files[c][path]; ok {
						object[field] = map[string]any{"object": map[string]any{"text": text}}
					}
				}
			}
			for field, key := range map[string]string{"dockerfile": "docker", "dockerfileLower": "dockerLower"} {
				if path, ok := str(key, i); ok {
					object[field] = nil
					if _, ok := repo.Files[c][path]; ok {
						object[field] = map[string]any{"oid": c}
					}
				}
			}
			data[alias] = map[string]any{"nameWithOwner": nwo, "object": object}
		}
	case parity:
		for i := 0; ; i++ {
			owner, ok := str("owner", i)
			if !ok {
				break
			}
			name, _ := str("name", i)
			alias := fmt.Sprintf("a%d", i)
			nwo, repo := h.repo(owner, name)
			if repo == nil {
				missing(alias)
				continue
			}
			oid, _ := str("oid", i)
			entry := map[string]any{"nameWithOwner": nwo, "databaseId": repo.RepoID, "owner": map[string]any{"databaseId": repo.OwnerID}, "commit": nil}
			if c := repo.peel(oid); c != "" && parserlock.IsFullSha(oid) {
				entry["commit"] = map[string]any{"oid": c}
			}
			for _, kind := range []string{"tag", "head"} {
				if ref, ok := str(kind+"Ref", i); ok {
					entry[kind], entry[kind+"Ref"] = nil, nil
					if c, ok := repo.Refs[ref]; ok {
						entry[kind], entry[kind+"Ref"] = map[string]any{"oid": c}, map[string]any{"name": ref}
					}
				}
			}
			data[alias] = entry
		}
	case reach:
		owner, _ := q.Variables["owner"].(string)
		name, _ := q.Variables["name"].(string)
		sha, _ := q.Variables["sha"].(string)
		_, repo := h.repo(owner, name)
		if repo == nil {
			missing("repo")
			break
		}
		branches := map[string]any{}
		for i := 0; ; i++ {
			ref, ok := str("ref", i)
			if !ok {
				break
			}
			branches[fmt.Sprintf("b%d", i)] = nil
			if c, ok := repo.Refs[ref]; ok {
				if head := repo.peel(sha); head != "" {
					branches[fmt.Sprintf("b%d", i)] = map[string]any{"compare": map[string]any{"status": strings.ToUpper(repo.compare(c, head))}}
				}
			}
		}
		data["repo"] = branches
	default:
		return 0, nil, false
	}
	out := map[string]any{"data": data}
	if len(errs) > 0 {
		out["errors"] = errs
	}
	return http.StatusOK, out, true
}

func kilnJSON(t *testing.T, path string, v any) {
	t.Helper()
	raw, err := os.ReadFile(path)
	require.NoError(t, err)
	require.NoError(t, json.Unmarshal(raw, v))
}

func kilnWrite(t *testing.T, path string, v any) {
	t.Helper()
	raw, err := json.MarshalIndent(v, "", "  ")
	require.NoError(t, err)
	require.NoError(t, os.WriteFile(path, append(raw, '\n'), 0o644))
}

// kilnScratch copies the case's tree, less any actions.lock, into a fresh directory the
// command runs in, with no gh login or run log of the person's.
func kilnScratch(t *testing.T, tree string) string {
	t.Helper()
	work := t.TempDir()
	require.NoError(t, filepath.WalkDir(tree, func(path string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() || d.Name() == "actions.lock" {
			return err
		}
		rel, err := filepath.Rel(tree, path)
		if err != nil {
			return err
		}
		raw, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		if err := os.MkdirAll(filepath.Dir(filepath.Join(work, rel)), 0o755); err != nil {
			return err
		}
		return os.WriteFile(filepath.Join(work, rel), raw, 0o644)
	}))
	t.Chdir(work)
	isolateRunLogs(t)
	t.Setenv("GH_ACTIONS_LOCK_REPO_ROOT", work)
	return work
}

// kilnReading is the official parser's reading of a lock: every field, and each workflow's
// closure over the uses edges.
func kilnReading(t *testing.T, lock []byte, path string) {
	t.Helper()
	f, err := parserlock.Parse(lock)
	if err != nil {
		kilnWrite(t, path, map[string]any{"error": err.Error()})
		return
	}
	dependencies := map[string]any{}
	for key, a := range f.Dependencies {
		dependencies[key] = map[string]any{"hostname": a.Hostname, "ref": a.Ref, "commit": a.Commit, "owner_id": a.OwnerID, "repo_id": a.RepoID, "uses": append([]string{}, a.Uses...)}
	}
	workflows, closures := map[string]any{}, map[string]any{}
	for w, roots := range f.Workflows {
		workflows[w] = append([]string{}, roots...)
		seen := map[string]bool{}
		for todo := append([]string{}, roots...); len(todo) > 0; {
			key := todo[len(todo)-1]
			todo = todo[:len(todo)-1]
			if !seen[key] {
				seen[key] = true
				todo = append(todo, f.Dependencies[key].Uses...)
			}
		}
		closure := []string{}
		for key := range seen {
			closure = append(closure, key)
		}
		sort.Strings(closure)
		closures[w] = closure
	}
	kilnWrite(t, path, map[string]any{"version": f.Version, "workflows": workflows, "dependencies": dependencies, "closure": closures})
}

func TestKilnInterop(t *testing.T) {
	cases, out := os.Getenv("KILN_INTEROP_CASES"), os.Getenv("KILN_INTEROP_OUT")
	if cases == "" || out == "" {
		t.Skip("KILN_INTEROP_CASES and KILN_INTEROP_OUT name kiln's cases and where their results go")
	}
	entries, err := os.ReadDir(cases)
	require.NoError(t, err)
	for _, entry := range entries {
		if entry.IsDir() {
			name := entry.Name()
			t.Run(name, func(t *testing.T) { kilnCase(t, filepath.Join(cases, name), filepath.Join(out, name)) })
		}
	}
}

func kilnCase(t *testing.T, dir, out string) {
	var facts kilnFacts
	kilnJSON(t, filepath.Join(dir, "facts.json"), &facts)
	var scenarios []kilnScenario
	kilnJSON(t, filepath.Join(dir, "scenarios.json"), &scenarios)
	require.NoError(t, os.MkdirAll(out, 0o755))
	tree := filepath.Join(dir, "tree")
	matches, err := filepath.Glob(filepath.Join(tree, ".github", "workflows", "*.y*ml"))
	require.NoError(t, err)
	workflows := []string{}
	for _, m := range matches {
		rel, err := filepath.Rel(tree, m)
		require.NoError(t, err)
		workflows = append(workflows, filepath.ToSlash(rel))
	}
	sort.Strings(workflows)

	var generated []byte
	t.Run("generate", func(t *testing.T) {
		work := kilnScratch(t, tree)
		host := &kilnHost{facts: facts}
		_, stderr, err := runCommandWithHTTP(t, host, append([]string{"--no-interactive", "--no-narrow"}, workflows...)...)
		host.done(t)
		require.NoError(t, err, stderr)
		generated, err = os.ReadFile(filepath.Join(work, ".github", "workflows", "actions.lock"))
		require.NoError(t, err)
		for _, w := range workflows {
			before, err := os.ReadFile(filepath.Join(tree, w))
			require.NoError(t, err)
			after, err := os.ReadFile(filepath.Join(work, w))
			require.NoError(t, err)
			require.True(t, bytes.Equal(before, after), "%s: the CLI rewrote a workflow", w)
		}
		require.NoError(t, os.WriteFile(filepath.Join(out, "actions.lock"), generated, 0o644))
		kilnReading(t, generated, filepath.Join(out, "read.json"))
	})
	require.NotEmpty(t, generated, "no lock to verify")

	verdicts := map[string]any{}
	for _, s := range scenarios {
		t.Run("verify/"+s.Name, func(t *testing.T) {
			lock := generated
			if s.Lock == "given" {
				given, err := os.ReadFile(filepath.Join(tree, ".github", "workflows", "actions.lock"))
				require.NoError(t, err)
				lock = given
				kilnReading(t, lock, filepath.Join(out, "read-given.json"))
			}
			work := kilnScratch(t, tree)
			require.NoError(t, os.WriteFile(filepath.Join(work, ".github", "workflows", "actions.lock"), lock, 0o644))
			var changed kilnFacts
			raw, err := json.Marshal(facts)
			require.NoError(t, err)
			require.NoError(t, json.Unmarshal(raw, &changed))
			for _, c := range s.Changes {
				switch {
				case c.Workflow != nil:
					path := filepath.Join(work, filepath.FromSlash(c.Workflow.Path))
					text, err := os.ReadFile(path)
					require.NoError(t, err)
					require.Contains(t, string(text), c.Workflow.From)
					require.NoError(t, os.WriteFile(path, []byte(strings.Replace(string(text), c.Workflow.From, c.Workflow.To, 1)), 0o644))
				case c.Move != nil:
					repo := changed.Repositories[c.Move.Repository]
					require.NotNil(t, repo, c.Move.Repository)
					repo.Commits[c.Move.To] = c.Move.Parents
					repo.Files[c.Move.To] = c.Move.Files
					repo.Refs[c.Move.Ref] = c.Move.To
				case c.Identity != nil:
					repo := changed.Repositories[c.Identity.Repository]
					require.NotNil(t, repo, c.Identity.Repository)
					repo.OwnerID, repo.RepoID = c.Identity.OwnerID, c.Identity.RepoID
				}
			}
			// --verify is the frozen check kiln's ci.locks.verify answers as. A read-only relock,
			// as upstream's own golden test runs it, also re-resolves each ref, which is where a
			// moved partial ref's warning comes from.
			verdict := map[string]any{}
			for mode, flags := range map[string][]string{"verify": {"--verify"}, "check": {"--relock", "--no-fix"}} {
				host := &kilnHost{facts: changed}
				stdout, stderr, err := runCommandWithHTTP(t, host, append(append(flags, "--json=valid,findings"), workflows...)...)
				host.done(t)
				var payload struct {
					Valid    bool             `json:"valid"`
					Findings []map[string]any `json:"findings"`
				}
				require.NoError(t, json.Unmarshal([]byte(stdout), &payload), "%s: stdout %q stderr %q", mode, stdout, stderr)
				findings := []map[string]any{}
				for _, f := range payload.Findings {
					findings = append(findings, map[string]any{"category": f["category"], "severity": f["severity"], "dependency": f["dependency"], "workflow": f["workflow"]})
				}
				sort.Slice(findings, func(i, j int) bool { return fmt.Sprint(findings[i]) < fmt.Sprint(findings[j]) })
				verdict[mode] = map[string]any{"valid": payload.Valid, "exit_ok": err == nil, "findings": findings}
			}
			after, err := os.ReadFile(filepath.Join(work, ".github", "workflows", "actions.lock"))
			require.NoError(t, err)
			require.True(t, bytes.Equal(lock, after), "a read-only run changed the lock")
			verdicts[s.Name] = verdict
		})
	}
	kilnWrite(t, filepath.Join(out, "verify.json"), verdicts)
}
