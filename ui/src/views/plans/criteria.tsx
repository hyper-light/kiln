/**
 * A step's acceptance criteria and tests. A criterion is in words, or a measure: what's measured
 * (its target), how it compares (at most, between, rises by at least … against a baseline), its
 * threshold and unit, and how it's measured; under each, the sentence the agent will read. A test
 * is a scenario and the criteria it passes by. Lists in the page's flow, the console's own fields
 * and selects; on a phone a measure's fields wrap, nothing scrolls sideways.
 */
import { X } from "lucide-react";
import type { Criterion, CriterionOp, PlanTest } from "@/api/plans";
import { SelectField } from "@/components/ui/misc";
import { criterionSentence } from "./steps";
import "./plans.css";

const OPS: { value: CriterionOp; label: string }[] = [
  { value: "le", label: "is at most (≤)" },
  { value: "lt", label: "is less than (<)" },
  { value: "ge", label: "is at least (≥)" },
  { value: "gt", label: "is more than (>)" },
  { value: "eq", label: "is exactly (=)" },
  { value: "ne", label: "is not (≠)" },
  { value: "between", label: "is between … and …" },
  { value: "within", label: "is within ± of" },
  { value: "rises_at_least", label: "rises by at least … %" },
  { value: "falls_at_least", label: "falls by at least … %" },
  { value: "rises_at_most", label: "rises by no more than … %" },
  { value: "falls_at_most", label: "falls by no more than … %" },
];
const KINDS = [
  { value: "qualitative", label: "In words" },
  { value: "quantitative", label: "A measure" },
];
const relative = (op: CriterionOp) => op.startsWith("rises") || op.startsWith("falls");

function Field({
  id,
  value,
  onChange,
  placeholder,
  className,
  numeric,
  label,
  hideLabel,
}: {
  id: string;
  value: string;
  onChange: (v: string) => void;
  placeholder: string;
  className?: string;
  numeric?: boolean;
  label: string;
  hideLabel?: boolean;
}) {
  return (
    <div className={className}>
      <label className={hideLabel ? "sr-only" : "field-label cr-label"} htmlFor={id}>
        {label}
      </label>
      <div className="field">
        <input id={id} value={value} placeholder={placeholder} inputMode={numeric ? "decimal" : undefined} onChange={(e) => onChange(e.target.value)} />
      </div>
    </div>
  );
}

export function CriteriaEditor({ criteria, onChange, idPrefix }: { criteria: Criterion[]; onChange: (c: Criterion[]) => void; idPrefix: string }) {
  const set = (i: number, c: Criterion) => onChange(criteria.map((x, j) => (j === i ? c : x)));
  const words = (): Criterion => ({ kind: "qualitative", text: "" });
  const measure = (): Criterion => ({ kind: "quantitative", target: "", op: "le", value: "", unit: "" });
  return (
    <div className="cr-wrap">
      {criteria.length > 0 && (
        <ol className="cr-list">
          {criteria.map((c, i) => {
            const id = `${idPrefix}-c${i}`;
            const reads = criterionSentence(c);
            return (
              <li key={i} className="cr" data-kind={c.kind}>
                <div className="cr-top">
                  <SelectField
                    className="cr-kind"
                    label={`Criterion ${i + 1}: in words, or a measure`}
                    hideLabel
                    value={c.kind}
                    onChange={(k) => set(i, k === "qualitative" ? { kind: "qualitative", text: c.kind === "qualitative" ? c.text : "" } : c.kind === "quantitative" ? c : measure())}
                    options={KINDS}
                  />
                  <button type="button" className="pi-icon" aria-label={`Remove criterion ${i + 1}`} onClick={() => onChange(criteria.filter((_, j) => j !== i))}>
                    <X size={13} />
                  </button>
                </div>
                {c.kind === "qualitative" ? (
                  <Field id={`${id}-text`} label="The criterion, in words" hideLabel value={c.text} onChange={(text) => set(i, { ...c, text })} placeholder="e.g. the error names the missing field" />
                ) : (
                  <div className="cr-measure">
                    <Field id={`${id}-target`} label="Target" value={c.target} onChange={(target) => set(i, { ...c, target })} placeholder="e.g. p99.9 latency" />
                    <div className="cr-row">
                      <SelectField className="cr-op" label="Compares" value={c.op} onChange={(op) => set(i, { ...c, op: op as CriterionOp })} options={OPS} />
                      {c.op === "within" ? (
                        <>
                          <Field id={`${id}-v`} className="cr-num" numeric label="Value" value={c.value} onChange={(value) => set(i, { ...c, value })} placeholder="value" />
                          <span className="cr-word">±</span>
                          <Field id={`${id}-v2`} className="cr-num" numeric label="Give or take" value={c.value2 ?? ""} onChange={(value2) => set(i, { ...c, value2 })} placeholder="give or take" />
                        </>
                      ) : (
                        <>
                          <Field
                            id={`${id}-v`}
                            className="cr-num"
                            numeric
                            label={relative(c.op) ? "Percent" : "Threshold"}
                            value={c.value}
                            onChange={(value) => set(i, { ...c, value })}
                            placeholder={relative(c.op) ? "%" : "threshold"}
                          />
                          {c.op === "between" && (
                            <>
                              <span className="cr-word">and</span>
                              <Field id={`${id}-v2`} className="cr-num" numeric label="Upper bound" value={c.value2 ?? ""} onChange={(value2) => set(i, { ...c, value2 })} placeholder="upper" />
                            </>
                          )}
                        </>
                      )}
                      {relative(c.op) ? (
                        <Field id={`${id}-base`} className="cr-base" label="Compared with" value={c.baseline ?? ""} onChange={(baseline) => set(i, { ...c, baseline })} placeholder="e.g. main" />
                      ) : (
                        <Field id={`${id}-unit`} className="cr-unit" label="Unit" value={c.unit ?? ""} onChange={(unit) => set(i, { ...c, unit })} placeholder="e.g. ms" />
                      )}
                    </div>
                    <Field id={`${id}-how`} label="Measured by (optional)" value={c.measured ?? ""} onChange={(measured) => set(i, { ...c, measured })} placeholder="e.g. the k6 soak test" />
                  </div>
                )}
                <p className="cr-reads" aria-label="How the agent reads it">
                  {reads.trim() ? <>&ldquo;{reads}&rdquo;</> : "Reads as a sentence once it has its words."}
                </p>
              </li>
            );
          })}
        </ol>
      )}
      <div className="cr-adds">
        <button type="button" className="pi-add" onClick={() => onChange([...criteria, words()])}>
          + In words
        </button>
        <button type="button" className="pi-add" onClick={() => onChange([...criteria, measure()])}>
          + A measure
        </button>
      </div>
    </div>
  );
}

export function TestsEditor({ tests, onChange, idPrefix }: { tests: PlanTest[]; onChange: (t: PlanTest[]) => void; idPrefix: string }) {
  const set = (i: number, t: PlanTest) => onChange(tests.map((x, j) => (j === i ? t : x)));
  return (
    <div className="cr-wrap">
      {tests.length > 0 && (
        <ol className="ts-list">
          {tests.map((t, i) => (
            <li key={i} className="ts">
              <div className="ts-top">
                <label className="field-label" htmlFor={`${idPrefix}-t${i}`}>
                  Test {i + 1}: the scenario
                </label>
                <button type="button" className="pi-icon" aria-label={`Remove test ${i + 1}`} onClick={() => onChange(tests.filter((_, j) => j !== i))}>
                  <X size={13} />
                </button>
              </div>
              <div className="field">
                <textarea
                  id={`${idPrefix}-t${i}`}
                  rows={2}
                  value={t.scenario}
                  placeholder="What's set up and done, e.g. upload 1 GB over a 10 Mbps link"
                  onChange={(e) => set(i, { ...t, scenario: e.target.value })}
                />
              </div>
              <div className="field-label ts-pass">Passes when</div>
              <CriteriaEditor criteria={t.criteria} onChange={(criteria) => set(i, { ...t, criteria })} idPrefix={`${idPrefix}-t${i}`} />
            </li>
          ))}
        </ol>
      )}
      <button type="button" className="pi-add" onClick={() => onChange([...tests, { scenario: "", criteria: [] }])}>
        + Add a test
      </button>
    </div>
  );
}
