#!/usr/bin/env bash
# End-to-end check: index tests/fixtures, run fixed queries, write tests/report/e2e-report.html.
# Usage: tests/e2e.sh        (MMSEARCH=<binary> to override target/release/mmsearch)
# MMSEARCH_SCREENSHOT=1 also captures the overlay with grim inside a Wayland session.
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
MM=${MMSEARCH:-$REPO/target/release/mmsearch}
REPORT_DIR=$REPO/tests/report
REPORT=$REPORT_DIR/e2e-report.html
QUERIES=$REPO/tests/queries

# Re-run inside the dev shell if a needed tool is missing.
# mmsearch itself calls ffmpeg, ffprobe and pdftotext while it indexes.
for tool in ffmpeg ffprobe pdftotext jq base64; do
  if ! command -v "$tool" >/dev/null; then
    [ -z "${E2E_IN_NIX:-}" ] || { echo "missing tool: $tool" >&2; exit 2; }
    exec env E2E_IN_NIX=1 nix develop "path:$REPO" --command "$0" "$@"
  fi
done
[ -x "$MM" ] || { echo "binary not found: $MM (cargo build --release first)" >&2; exit 2; }

TMP=$(mktemp -d)
# The indexer writes thumbnails; keep them out of the real ~/.cache/thumbnails. The GPU
# shader caches also live under XDG_CACHE_HOME; keep those, or every run recompiles them.
export __GL_SHADER_DISK_CACHE_PATH=${__GL_SHADER_DISK_CACHE_PATH:-${XDG_CACHE_HOME:-$HOME/.cache}/nvidia}
export MESA_SHADER_CACHE_DIR=${MESA_SHADER_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/mesa_shader_cache}
export XDG_CACHE_HOME=$TMP/cache
DAEMON_PID=
cleanup() {
  [ -z "$DAEMON_PID" ] || kill "$DAEMON_PID" 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

ROOT=$TMP/root
DB=$TMP/index.db
mkdir -p "$ROOT" "$REPORT_DIR" "$TMP/thumbs"
cp -r "$REPO"/tests/fixtures/. "$ROOT"/
rm -f "$ROOT/SOURCES.txt"

echo "indexing $ROOT"
"$MM" index --root "$ROOT" --db "$DB"

# label | kind of input | input | expected file
CASES=(
  "text|a cat sitting among green plants|IMG_0412.jpg"
  "text|ocean waves breaking on a beach|IMG_0587.jpg"
  "text|a vintage blue convertible sports car|IMG_0733.jpg"
  "text|forested mountain slope under a clear blue sky|IMG_0891.jpg"
  "text|a speech asking what you can do for your country|recording_01.wav"
  "text|a sea turtle swimming over coral|clip_01.mp4"
  "text|total amount due on the plumber invoice|invoice.pdf"
  "text|parse_config_line|util.rs"
  "text|how to make pancakes with maple syrup|note_a.md"
  "image|image-query.jpg|IMG_0412.jpg"
  "audio|audio-query.wav|recording_01.wav"
)

esc() { jq -rn --arg s "$1" '$s | @html'; }

# Small inline jpeg for images and the first frame of a video; empty for other kinds.
thumb_b64() {
  local path=$1 kind=$2 out=$TMP/thumbs/t.jpg
  case $kind in
    image|video) ;;
    *) return 0 ;;
  esac
  [ -f "$path" ] || return 0
  rm -f "$out"
  ffmpeg -v error -y -i "$path" -frames:v 1 -vf "scale=96:-2" "$out" 2>/dev/null || return 0
  [ -s "$out" ] && base64 -w0 "$out"
}

BODY=$TMP/body.html
: > "$BODY"
pass=0
fail=0
n=0

for entry in "${CASES[@]}"; do
  IFS='|' read -r mode input expected <<<"$entry"
  n=$((n + 1))
  json=$TMP/q$n.json
  case $mode in
    text)  "$MM" query "$input" --db "$DB" --limit 5 --json >"$json"; shown=$input ;;
    image) "$MM" query --image "$QUERIES/$input" --db "$DB" --limit 5 --json >"$json"; shown="image: $input" ;;
    audio) "$MM" query --audio "$QUERIES/$input" --db "$DB" --limit 5 --json >"$json"; shown="audio: $input" ;;
  esac

  if jq -e --arg e "$expected" '[.[:3][].path | split("/")[-1]] | index($e) != null' "$json" >/dev/null; then
    status=PASS; pass=$((pass + 1))
  else
    status=FAIL; fail=$((fail + 1))
  fi
  top=$(jq -r '.[0].path // "-" | split("/")[-1]' "$json")
  printf '%s  %-60s expect %-18s top1 %s\n' "$status" "$shown" "$expected" "$top"

  {
    echo "<section class=\"$status\"><h2><span class=\"badge\">$status</span> $(esc "$shown")</h2>"
    echo "<p class=\"exp\">expected in top 3: $(esc "$expected")</p>"
    echo "<table><tr><th>#</th><th>score</th><th>kind</th><th>file</th><th></th></tr>"
    jq -r '.[:5] | to_entries[] | [(.key + 1), .value.score, .value.kind, .value.path] | @tsv' "$json" |
      while IFS=$'\t' read -r rank score kind path; do
        abs=$path
        case $path in /*) ;; *) abs=$ROOT/$path ;; esac
        name=$(basename "$path")
        b64=$(thumb_b64 "$abs" "$kind")
        img=
        [ -z "$b64" ] || img="<img src=\"data:image/jpeg;base64,$b64\" alt=\"\">"
        hit=
        [ "$name" != "$expected" ] || hit=" class=\"want\""
        printf '<tr%s><td>%s</td><td>%.5f</td><td>%s</td><td>%s</td><td>%s</td></tr>\n' \
          "$hit" "$rank" "$score" "$(esc "$kind")" "$(esc "$name")" "$img"
      done
    echo "</table></section>"
  } >>"$BODY"
done

SHOT_HTML=
if [ -n "${WAYLAND_DISPLAY:-}" ] && [ "${MMSEARCH_SCREENSHOT:-}" = 1 ]; then
  case $WAYLAND_DISPLAY in
    /*) wl=$WAYLAND_DISPLAY ;;
    *)  wl=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR unset}/$WAYLAND_DISPLAY ;;
  esac
  mkdir -p "$TMP/data/mmsearch/models" "$TMP/run"
  chmod 700 "$TMP/run"
  ln -s "$HOME"/.local/share/mmsearch/models/* "$TMP/data/mmsearch/models/"
  cp "$DB" "$TMP/data/mmsearch/index.db"
  env HOME="$ROOT" XDG_DATA_HOME="$TMP/data" XDG_RUNTIME_DIR="$TMP/run" \
      WAYLAND_DISPLAY="$wl" MMSEARCH_NO_GRAB=1 "$MM" daemon >"$TMP/daemon.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 60); do [ -S "$TMP/run/mmsearch.sock" ] && break; sleep 0.5; done
  if [ -S "$TMP/run/mmsearch.sock" ]; then
    XDG_RUNTIME_DIR="$TMP/run" "$MM" toggle
    sleep 2
    # Capture only the 900x560 panel at the center of the focused output: the rest of the
    # screen is the user's private desktop. Without niri there is no geometry, so no shot.
    geom=$(niri msg --json focused-output 2>/dev/null |
      jq -r '.logical | "\(.x + ((.width - 900) / 2 | floor)),\(.y + ((.height - 560) / 2 | floor)) 900x560"' 2>/dev/null || true)
    if [ -n "$geom" ] && grim -g "$geom" "$REPORT_DIR/overlay.png"; then
      SHOT_HTML="<section><h2>Overlay</h2><img class=\"shot\" src=\"data:image/png;base64,$(base64 -w0 "$REPORT_DIR/overlay.png")\" alt=\"overlay screenshot\"></section>"
    else
      echo "no niri output geometry; skipping the overlay screenshot" >&2
    fi
  else
    echo "daemon socket did not appear; see log:" >&2
    cat "$TMP/daemon.log" >&2
  fi
fi

total=$((pass + fail))
cat >"$REPORT" <<EOF
<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>mmsearch e2e report</title>
<style>
body{font:14px/1.4 system-ui,sans-serif;max-width:900px;margin:2em auto;padding:0 1em;background:#fafafa;color:#222}
section{background:#fff;border:1px solid #ddd;border-left:6px solid #888;border-radius:4px;padding:.5em 1em;margin:1em 0}
section.PASS{border-left-color:#2a9d4a}section.FAIL{border-left-color:#d33}
h2{font-size:1em;margin:.4em 0}.badge{font-weight:700;padding:1px 6px;border-radius:3px;color:#fff;background:#888}
.PASS .badge{background:#2a9d4a}.FAIL .badge{background:#d33}
.exp{color:#666;margin:.2em 0}table{border-collapse:collapse;width:100%}
td,th{padding:3px 8px;text-align:left;border-top:1px solid #eee;vertical-align:middle}tr.want{background:#e6f6ea}
img{display:block;height:48px}img.shot{height:auto;max-width:100%}
</style></head><body>
<h1>mmsearch e2e: $pass of $total passed</h1>
<p>Index: $(esc "$(basename "$ROOT")") with $(find "$ROOT" -type f | wc -l) files. A query passes when its expected file is in the top 3.</p>
$(cat "$BODY")
$SHOT_HTML
</body></html>
EOF

echo "report: $REPORT"
echo "$pass of $total queries passed"
[ "$fail" -eq 0 ]
