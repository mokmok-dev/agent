#!/usr/bin/env bash
set -euo pipefail

baseline=${1:-nix/complexity-baseline.json}
report=$(mktemp)
trap 'rm -f "$report"' EXIT

cccc --no-config --lang rust --pretty crates > "$report"

failures=$(jq -r --slurpfile base "$baseline" '
  def fns: .[] as $f | $f, (($f.children // []) | fns);
  ($base[0]) as $b
  | ($b.thresholds) as $t
  | [ .files[] | .path as $path | (.functions | fns)
      | { path: $path, name: .name, cognitive: .cognitive, cyclomatic: .cyclomatic } ] as $measured
  | ($measured | map(select(.cognitive > $t.cognitive or .cyclomatic > $t.cyclomatic))) as $over
  | (
      [ $b.allowed[] as $entry
        | ($measured | map(select(.path == $entry.path and .name == $entry.name)) | first) as $fn
        | if $fn == null then
            "\($entry.path): \($entry.name) is in the baseline but no such function exists; remove the entry"
          elif ($fn.cognitive <= $t.cognitive and $fn.cyclomatic <= $t.cyclomatic) then
            "\($entry.path): \($entry.name) now passes the gate at \($fn.cognitive)/\($fn.cyclomatic); remove the baseline entry"
          elif ($fn.cognitive > $entry.cognitive or $fn.cyclomatic > $entry.cyclomatic) then
            "\($entry.path): \($fn.name) grew to \($fn.cognitive) cognitive, \($fn.cyclomatic) cyclomatic; the baseline allows \($entry.cognitive)/\($entry.cyclomatic)"
          elif ($fn.cognitive < $entry.cognitive or $fn.cyclomatic < $entry.cyclomatic) then
            "\($entry.path): \($fn.name) improved to \($fn.cognitive) cognitive, \($fn.cyclomatic) cyclomatic; lower the baseline entry from \($entry.cognitive)/\($entry.cyclomatic)"
          else empty end
      ]
      + [ $over[] as $fn
          | select(([ $b.allowed[] | select(.path == $fn.path and .name == $fn.name) ] | length) == 0)
          | "\($fn.path): \($fn.name) is \($fn.cognitive) cognitive, \($fn.cyclomatic) cyclomatic, over the \($t.cognitive)/\($t.cyclomatic) gate, with no baseline entry"
        ]
      + (
          if .summary.parse_error_count > 0 then
            [ "cccc could not parse \(.summary.parse_error_file_count) file(s): \(.summary.parse_error_files | join(", "))" ]
          else [] end
        )
    ) as $failures
  | if ($failures | length) > 0 then $failures[] else empty end
' "$report")

if [[ -n "$failures" ]]; then
  printf '%s\n' "$failures" >&2
  exit 1
fi

jq -r --slurpfile base "$baseline" '
  "complexity: \(.summary.function_count) functions, max \(.summary.cognitive.max)/\(.summary.cyclomatic.max), gate \($base[0].thresholds.cognitive)/\($base[0].thresholds.cyclomatic), \($base[0].allowed | length) baseline entries"
' "$report"
