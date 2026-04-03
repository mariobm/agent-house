#!/bin/bash
# scripts/inline-spec.sh — Converts docs/openapi.yaml to JSON and inlines it
# into web/api-docs.html from the template (web/api-docs.tmpl.html).
#
# This makes the Scalar page fully self-contained — no fetch() needed,
# works when opened as file://, no CORS issues, single HTML file.
set -euo pipefail

SPEC_YAML="docs/openapi.yaml"
TEMPLATE="web/api-docs.tmpl.html"
OUTPUT="web/api-docs.html"

# Convert YAML to JSON
SPEC_JSON=$(npx --yes js-yaml "$SPEC_YAML" 2>/dev/null)

if [ -z "$SPEC_JSON" ]; then
  echo "ERROR: Failed to convert $SPEC_YAML to JSON" >&2
  exit 1
fi

# Replace placeholder and write output
python3 -c "
import sys

with open('$TEMPLATE') as f:
    html = f.read()

spec_json = sys.stdin.read().strip()
html = html.replace('%%SPEC_JSON%%', spec_json)

with open('$OUTPUT', 'w') as f:
    f.write(html)
" <<< "$SPEC_JSON"
