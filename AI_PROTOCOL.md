# tgsort ⇄ tgsort-ai protocol (v1)

`tgsort-ai` is a small Swift CLI using Apple's on-device Foundation Models.
tgsort spawns it only when needed; it exits after each call (zero idle cost).

## `tgsort-ai --check`
stdout, one JSON line, exit 0:
`{"available": true}` or `{"available": false, "reason": "appleIntelligenceNotEnabled"}`
(reason = the SystemLanguageModel.Availability unavailable reason, or "unsupportedOS").

## `tgsort-ai classify`
stdin: one JSON object, stdout: one JSON object, exit 0 (non-zero only on unusable input).
Request:
```json
{"files": [{"id": "0", "path": "/abs/file.pdf", "name": "file.pdf"}],
 "choices": [{"name": "Papers", "hint": "academic papers, research articles"}]}
```
Response (same order as request, one entry per file):
```json
{"results": [{"id": "0", "choice": "Papers"}, {"id": "1", "choice": null, "error": "..."}]}
```
- `choice` is exactly one of the request's choice names, or null when the model is unsure,
  unavailable, or failed for that file (then `error` may explain).
- The helper reads at most ~1500 chars of content: PDF text of the first 2 pages (PDFKit),
  or the start of plain-text files (txt, md, json, csv, html, …); otherwise name only.
- Content never leaves the device.
