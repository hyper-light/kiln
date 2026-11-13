#!/bin/bash
# fetch.sh NAME URL: curl -L URL into a new empty directory ~/.cache/g121-s18-hosts/dl/NAME/, as data only.
set -u
name=$1; url=$2
dir=$HOME/.cache/g121-s18-hosts/dl/$name
if [ -e "$dir" ]; then echo "EXISTS $dir"; exit 1; fi
mkdir -p "$dir"
file=$(basename "${url%%\?*}"); [ -z "$file" ] || [ "$file" = "/" ] && file=index
out="$dir/$file"
code=$(curl -L -sS --max-time 60 -o "$out" -w '%{http_code} %{content_type} %{url_effective}' "$url" 2>"$dir/.curl.err")
rc=$?
printf '%s\t%s\t%s\n' "$url" "$(date -u +%Y-%m-%d)" "$code" > "$dir/.meta"
if [ $rc -ne 0 ]; then echo "FAIL rc=$rc $name $url $(cat $dir/.curl.err)"; exit 1; fi
echo "$name | $code | $(wc -c < "$out")B | sha256 $(shasum -a 256 "$out" | cut -d' ' -f1) | $out"
