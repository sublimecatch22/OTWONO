# Education

**Status:** `SPECIFIED`. No implementation. Depends on Phase 4 (local AI, largely done),
Phase 5 (content store), Phase 6 (distributed services) and Phase 7 (agent layer). Targeted
at Phase 7.

---

## 1. What it is

A teacher that runs on the household's own machine: it plans lessons, sets practice, marks
work, and keeps a picture of what a learner actually knows — offline, free, and with the
records on the learner's own disk.

The three things it produces:

1. **Curriculum** — lesson plans, explanations, worked examples, practice sets, assessments.
2. **A model of the learner** — per-skill mastery estimates, updated from evidence, with the
   evidence retained.
3. **A portable record** — a signed, tamper-evident transcript the learner owns and can show
   to whoever needs to see it.

## 2. Why this fits the OS rather than being an app

Three properties it needs are OS properties here, not features to be re-implemented:

- **It works offline.** A household with intermittent Internet gets an uninterrupted school.
  Local inference (ADR-0011) is what makes that possible.
- **The records are the learner's.** Years of a child's academic history is among the most
  sensitive data a family holds. `PRIVATE` by default with no telemetry, ever, is the
  project's existing rule and it is the right one here.
- **The heavy content is shared efficiently.** Curriculum, textbooks and media are `PUBLIC`
  or `REPLICATED`, so a school district or a street of families pulls them once between them
  (`CLUSTER-CACHE.md`). Learner records are `PRIVATE` and never enter that path.

## 3. Data model, and the label on each part

| Object | Label | Note |
|---|---|---|
| Curriculum, lessons, practice banks | `PUBLIC` / `REPLICATED` | Shared, cached, verified by hash like any content |
| Learner profile, mastery estimates | `PRIVATE` | Never leaves the node without an explicit, logged export |
| Submitted work and marks | `PRIVATE` | Retained as the evidence behind every claim in the record |
| Signed transcript | `PRIVATE`, exportable | Promotion to `SHARED` is a deliberate user action, per ADR-0007 |

**A learner record is never `REPLICATED`.** Not as a backup, not for convenience, not for
"sync". If that is ever wanted it is an explicit encrypted export the user performs.

### Skill tracking

Mastery is per-skill and evidence-backed: a claim that a learner understands linear
equations points at the specific attempts that support it, with dates. Two rules that keep
this honest:

- **Decay.** Mastery demonstrated a year ago and not since is reported as stale, not as
  current. A model that only ever goes up is flattery, not assessment.
- **Uncertainty is shown.** Three correct answers is not mastery, and the interface says so
  rather than rendering a confident number.

## 4. Certification: what software can and cannot do

The goal is that this becomes "a certified way to go through school or home school". Being
plain about the split:

**What this system cannot do.** It cannot accredit itself. Accreditation is a relationship
with a body that has standing — a state education department, a regional accreditor, an
examination board. No amount of cryptography creates that standing, and any product that
implies otherwise is misleading the family relying on it.

**What this system can do**, and what is worth building:

- **A tamper-evident record.** Every assessment event hash-chained and signed by the node
  identity (the same construction as the audit log), so a transcript can be checked for
  alteration after the fact.
- **A record in the shape an authority wants.** Homeschool requirements differ by
  jurisdiction — hours logged, subjects covered, standardised test results, portfolio
  review, annual assessment. These belong in the system as **data**, not code: jurisdiction
  profiles that state what must be recorded and what a compliant report looks like. Exactly
  the pattern **OQ-10** already anticipates for radio regulations.
- **Alignment to published standards.** Mapping skills to an existing framework (Common
  Core, national curricula, exam board syllabi) is what makes a record legible to an
  outside reviewer.
- **Exports an institution can consume.** A portfolio a reviewer can read, and a transcript
  a registrar can file.

**The integrity limit, stated plainly.** If the AI teaches, sets the work, and marks it, on
a machine the learner controls, then the record is *self-attested*. A signature proves what
the node recorded. It does not prove the learner did the work unaided. That gap is why
external examinations exist, and no local system closes it. What the system can honestly
offer is a proctoring mode with recorded conditions, and a record that distinguishes
supervised assessments from unsupervised practice — so a reviewer can weigh them
differently. It should never present the two as the same evidence.

Recorded as **OQ-18**.

## 5. Tier behaviour

Per CLAUDE.md §2.6, availability comes from the capability profile, not from local guesses:

| Tier | What a learner gets |
|---|---|
| T0 | Practice, marking of structured answers, record-keeping. Curriculum is pre-generated and cached, not authored on the node |
| T1 | Small-model tutoring: explanation, hints, short-answer marking |
| T2 | Lesson planning, essay feedback, spoken practice via ASR/TTS |
| T3 | Long-context planning across a whole year; media generation |

A T0 node is a complete school with a smaller teacher — not a broken one. Degrading to
"pre-generated curriculum plus record-keeping" is a designed state and must be tested as one.

## 6. Safety, because the user may be a child

Non-negotiable, and to be treated as requirements rather than aspirations:

- **A model that can call tools is executable content.** The existing publisher-trust rule
  applies with no relaxation: unsigned or unknown-publisher models run with reduced tool
  access.
- **Curriculum arriving over the network is untrusted data, never instruction.** A lesson
  file cannot direct the agent. This is the §2.5 rule and it is exactly the attack surface
  a shared curriculum creates.
- **Hallucination is a safety issue in a teaching context**, not a quality issue. A confident
  wrong explanation given to someone with no way to detect it is the central risk of the
  whole idea. Mitigations — citation to source material in the cache, marking generated
  content as generated, a review path for a parent or teacher — are requirements, not
  polish.
- **No telemetry. Ever.** Already the project rule; restated because education products
  routinely violate it.

## 7. What must be true before this is called done

- A learner works through a unit, is assessed, and the record reflects it — offline, on a
  T1 node, with a log.
- A `PRIVATE` learner record cannot be replicated by any code path, proven negatively.
- A transcript export verifies, and a tampered one fails verification.
- Curriculum from a peer cannot alter agent behaviour, proven with a hostile fixture.
- The same unit works on a T0 node with pre-generated content.
