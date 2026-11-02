// Stack traces as Go's tracers report them, for kiln's error grouping tests: dd-trace-go's
// `error.stack` (its takeStacktrace: each frame's function, then a tab, its file and line), and
// a recovered panic's runtime/debug.Stack() (OpenTelemetry Go's WithStackTrace).
package main

import (
	"errors"
	"fmt"
	"runtime"
	"runtime/debug"
	"strconv"
	"strings"
)

// As dd-trace-go v1 ddtrace/tracer/span.go takeStacktrace (skip 0: from its caller).
func takeStacktrace(n, skip uint) string {
	pcs := make([]uintptr, n)
	numFrames := runtime.Callers(2+int(skip), pcs)
	if numFrames == 0 {
		return ""
	}
	frames := runtime.CallersFrames(pcs[:numFrames])
	var builder strings.Builder
	for i := 0; ; i++ {
		frame, more := frames.Next()
		if i != 0 {
			builder.WriteByte('\n')
		}
		builder.WriteString(frame.Function)
		builder.WriteByte('\n')
		builder.WriteByte('\t')
		builder.WriteString(frame.File)
		builder.WriteByte(':')
		builder.WriteString(strconv.Itoa(frame.Line))
		if !more {
			break
		}
	}
	return builder.String()
}

type cart struct{ items []int }

func (c *cart) total() (int, error) {
	for _, i := range c.items {
		if i < 0 {
			return 0, fmt.Errorf("bad item %d in cart %q", i, fmt.Sprint("c-", i*7))
		}
	}
	return 0, nil
}

func checkout(order int) string {
	c := &cart{items: []int{1, 2, -order}}
	if _, err := c.total(); err != nil {
		return takeStacktrace(32, 0)
	}
	return ""
}

func handler(id int) (stack string) {
	defer func() {
		if r := recover(); r != nil {
			stack = string(debug.Stack())
		}
	}()
	work := func() {
		var m map[string]int
		m["order"+strconv.Itoa(id)] = id
	}
	work()
	return ""
}

func parse(s string) string {
	if _, err := strconv.Atoi(s); err != nil {
		var ne *strconv.NumError
		if errors.As(err, &ne) {
			return takeStacktrace(32, 0)
		}
	}
	return ""
}

func main() {
	fmt.Printf("---- app\n%s\n", checkout(42))
	fmt.Printf("---- app-again\n%s\n", checkout(7))
	fmt.Printf("---- panic\n%s", handler(5))
	fmt.Printf("---- parse\n%s\n", parse("x1"))
}
