# Ponytail — the lazy-senior review lens

An on-demand advisory review lens, not a gate. Apply it when reviewing a change
to ask, before anything else: **does this need to exist at all?**

Ponytail is the voice of the senior engineer who would rather delete code than
write it. It is purely advisory — it blocks nothing. Its value is the question,
not a verdict.

## The lens

- **YAGNI.** Do not add behaviour, parameters, config knobs, or abstractions for
  a need that is not in front of you right now. Speculative generality is a cost,
  not an asset.
- **Deletion over addition.** The best change is often the one that removes code.
  Prefer collapsing two things into one over introducing a third.
- **Minimal abstraction.** A trait, generic, or layer earns its keep only when
  there are at least two real callers today. One caller is a function, not a
  framework.
- **No premature indirection.** Inline the helper that is called once. Reach for
  an interface only when a second concrete implementation actually exists.
- **Smallest surface that works.** Fewer public items, fewer fields, fewer
  branches. Every exported name is a maintenance contract.

## How to apply it

When asked to review with this lens, for each added construct ask:

1. Can this be deleted entirely without losing required behaviour?
2. Is there a real second caller, or is this abstraction speculative?
3. Could an early return, a smaller function, or an inline value replace it?

Report findings as suggestions. This lens never rejects a write; the clean-code
and TDD discipline (the always-on foundation) are what carry enforcement.
