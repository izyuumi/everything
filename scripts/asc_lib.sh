: "${ASC_KEY_ID:?ASC_KEY_ID is required}" "${ASC_ISSUER_ID:?ASC_ISSUER_ID is required}"
KEY=${ASC_KEY_PATH:-$HOME/.appstoreconnect/private_keys/AuthKey_$ASC_KEY_ID.p8}
case "$KEY" in /*) ;; *) KEY="$PWD/$KEY" ;; esac
[ -r "$KEY" ] || { echo "Cannot read the App Store Connect key at $KEY" >&2; exit 1; }
APP_ID=6829295744
API=https://api.appstoreconnect.apple.com
export APP_ID API

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

jwt() {
    now=$(date +%s)
    header=$(printf '{"alg":"ES256","kid":"%s","typ":"JWT"}' "$ASC_KEY_ID" | b64url)
    claims=$(printf '{"iss":"%s","iat":%s,"exp":%s,"aud":"appstoreconnect-v1"}' "$ASC_ISSUER_ID" "$now" $((now + 1200)) | b64url)
    signature=$(printf '%s.%s' "$header" "$claims" | openssl dgst -sha256 -sign "$KEY" | openssl asn1parse -inform DER \
        | awk -F: '/INTEGER/ { s = $NF; while (length(s) < 64) s = "0" s; printf "%s", substr(s, length(s) - 63) }' \
        | xxd -r -p | b64url)
    printf '%s.%s.%s' "$header" "$claims" "$signature"
}

asc_get() {
    printf 'header = "Authorization: Bearer %s"\n' "$(jwt)" | curl -fsS --config - "$1"
}

asc_send() {
    asc_body=$(mktemp) || return 1
    asc_status=$(printf 'header = "Authorization: Bearer %s"\nheader = "Content-Type: application/json"\n' "$(jwt)" \
        | curl -sS --config - -X "$1" --data "$3" -o "$asc_body" -w '%{http_code}' "$2") \
        || { asc_rc=$?; rm -f "$asc_body"; return "$asc_rc"; }
    case "$asc_status" in
        2??) cat "$asc_body"; rm -f "$asc_body"; return 0 ;;
    esac
    echo "App Store Connect answered HTTP $asc_status to $1 ${2%%\?*}" >&2
    asc_errors=$(jq -r '.errors[]? | "  \(.status // "?") \(.code // "?"): \(.title // "")"
        + (if .detail then " - \(.detail)" else "" end)' "$asc_body" 2>/dev/null) || asc_errors=
    if [ -n "$asc_errors" ]; then printf '%s\n' "$asc_errors" >&2; else head -c 500 "$asc_body" >&2; echo >&2; fi
    rm -f "$asc_body"
    return 22
}
