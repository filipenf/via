# algo fixture (bisect / flatten / gcd)

Issue: three small Lua helpers misbehave on real inputs. Each module looks
mostly correct — many smoke cases already pass — but the full suite does not.

| File | Entry point |
| --- | --- |
| `bisect.lua` | `find_first_in_sorted` |
| `flatten.lua` | `flatten` |
| `gcd.lua` | `gcd` |

(Defect shapes are QuixBugs-class; the eval docs mention that provenance — the
library names themselves do not.)

## Acceptance

From this directory:

```sh
./verify.sh
```

Must exit 0. Tests live in `test_algo.lua` and run under `nvim -l`.

Test output is labeled:

- `PASS_TO_PASS` — already green on the buggy tree; must stay green (do not
  regress with a careless rewrite).
- `FAIL_TO_PASS` — currently failing; these define the defects to repair.

## Constraints

- Fix the defects in `bisect.lua`, `flatten.lua`, and/or `gcd.lua`. Prefer
  surgical repairs over from-scratch rewrites so PASS_TO_PASS cases keep
  passing.
- Do not modify `test_algo.lua` or `verify.sh`.
- Scope is this directory only (plus via task/agent CLI for planning and review).

## Review artifact

Reviewers write durable findings to `REVIEW.md` in this directory (see the eval
`PROMPT.md` template). That file is created by the eval run; it is not shipped
as part of the fixture.
