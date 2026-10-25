# Directory Update Log

## 2026-10-25
* **Creation**: Established the spec bundle with product scope, architecture, interfaces, and delivery sections.
* **Creation**: Recorded the event model, store format, and branch semantics as the three load-bearing concepts.
* **Update**: Removed worked code from every concept. The bundle states policy and shape, and leaves the implementation to the PRs it describes.
* **Update**: Drew the architecture and branch diagrams in Mermaid, so they render and stay legible when the bundle is read in a browser.
* **Update**: Replaced the framed segment format with one SQLite database. The custom format owned durability code that SQLite already provides, and the reversal is recorded in [decisions](architecture/decisions.md).
* **Update**: Strengthened the no-merge decision with the reason that decides it. A prompt is a linear token sequence, and a two-parent history has no canonical linearization.
* **Update**: Promoted every concept from draft to stable after the operator's review, and recorded the review in `verified`.
