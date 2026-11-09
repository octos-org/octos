# Authoring capability evaluation

Unit tests establish invariants, not model or visual parity. Keep production
page data, accepted answers and private decks out of this package.

Before a live comparison, record immutable inputs, model/reasoning setting,
image-delivery policy, tool/skill hashes, editable scope and acceptance path.
Both runs receive the same source content and design/blank images. Do not give
one model reviewer answers or require only one model to clear unsupported checks.
Separate repeated benchmark pages from held-out pages from a different deck;
do not tune implementation using held-out answers during that run.

Capture the first readable actual PPTX render before independent feedback,
then each completed repair cycle and the final artifact. The reviewer inspects
full pages as well as matched original/blank/current context crops. Use stable
defect IDs and impact-based criteria: wrong/missing copy, containment, owner
association, orientation, consistency and readability. Subjective preferences
and unsupported measurements are recorded separately.

Report initial and remaining verified defects, correct repairs, regressions,
missed defects and false alarms against an independently labeled review set.
Do not infer recall where the review set is not complete. Also record time to
first artifact, total wall time, completed cycles, actual peer feedback timing,
model requests, generated tokens, cached and uncached input when reported.
Missing usage is unknown; total input is not all newly computed input.

A smaller unknown count, successful build, or fewer native boxes is not evidence
of better layout. Inspect the actual PPTX export. Describe what was verified and
what remains untested; do not generalize two-page success to all decks/models.
