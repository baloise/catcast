#!/usr/bin/env bash
# Corp-proxy probe (curl version). Run from WSL or Linux:
#
#     ./probe.sh | tee probe-results.txt
#
# Note: WSL networking may or may not be subject to the same corp proxy as
# native Windows. The PowerShell version (probe.ps1) is the more reliable
# test for the actual stage deployment target.

set -u

cases=(
  "1. GH Release binary (.zip)|https://github.com/cli/cli/releases/download/v2.50.0/gh_2.50.0_windows_amd64.zip|binary"
  "2. GH API (JSON)|https://api.github.com/repos/cli/cli/releases/latest|text"
  "3. raw.githubusercontent.com (text)|https://raw.githubusercontent.com/cli/cli/trunk/README.md|text"
  "4. objects.githubusercontent.com (release blob CDN)|https://objects.githubusercontent.com/|text"
  "5. workers.dev (CF Worker)|https://workers.cloudflare.com/|text"
  "6. example.com (sanity)|https://example.com/|text"
)

probe() {
  local name="$1" url="$2" kind="$3"
  echo "=== $name ==="
  echo "URL: $url"
  local headers body status final_url content_type content_length
  if ! headers=$(curl -sSIL --max-time 30 -A "catcast-probe/0.1" "$url" 2>&1); then
    echo "error: $headers"
    echo "VERDICT: FAIL"
    echo
    return
  fi
  status=$(echo "$headers" | awk 'BEGIN{IGNORECASE=1} /^HTTP\//{s=$2} END{print s}')
  final_url=$(curl -sSILo /dev/null -w '%{url_effective}' --max-time 30 "$url" 2>/dev/null)
  content_type=$(echo "$headers" | awk 'BEGIN{IGNORECASE=1} /^content-type:/{ $1=""; print; exit }' | sed 's/^ //')
  content_length=$(echo "$headers" | awk 'BEGIN{IGNORECASE=1} /^content-length:/{ print $2; exit }' | tr -d '\r')
  echo "status:         $status"
  echo "final-url:      $final_url"
  echo "content-type:   $content_type"
  echo "content-length: $content_length"

  # Fetch first 120 bytes to sniff interstitials.
  body=$(curl -sSL --max-time 30 --range 0-119 "$url" 2>/dev/null || true)
  if [[ "$kind" == "binary" ]]; then
    local hex
    hex=$(printf '%s' "$body" | head -c 16 | xxd -p 2>/dev/null || true)
    echo "first-bytes-hex: $hex"
    if printf '%s' "$body" | grep -qiE '<html|<\?xml'; then
      echo "VERDICT: BLOCKED — HTML where binary was expected (proxy interstitial?)"
      echo
      return
    fi
  else
    local snippet
    snippet=$(printf '%s' "$body" | tr '\r\n' '  ' | head -c 120)
    echo "first-120-chars: $snippet"
  fi
  echo "VERDICT: PASS"
  echo
}

echo "CatCast corp-proxy probe — $(date -Iseconds)"
echo "Host:                  $(hostname)"
echo "HTTP_PROXY:            ${HTTP_PROXY:-}"
echo "HTTPS_PROXY:           ${HTTPS_PROXY:-}"
echo

for c in "${cases[@]}"; do
  IFS='|' read -r name url kind <<<"$c"
  probe "$name" "$url" "$kind"
done
