#!/usr/bin/env bash
# Shared helper for boundary rules: resolves a module name to its source files.
# A module may exist as a flat file (src/<module>.rs), a directory of files (src/<module>/**/*.rs),
# or both.
# Prints one path per line and exits/returns non-zero (exit 2) if neither exists.
set -uo pipefail

module_files() {
    local module="$1"
    local found=0
    if [ -f "src/${module}.rs" ]; then
        echo "src/${module}.rs"
        found=1
    fi
    if [ -d "src/${module}" ]; then
        while IFS= read -r file; do
            if [ -n "$file" ]; then
                echo "$file"
                found=1
            fi
        done < <(find "src/${module}" -name '*.rs' -type f | sort)
    fi
    if [ "$found" -eq 0 ]; then
        echo "module_files: no src/${module}.rs or src/${module}/ found" >&2
        return 2
    fi
    return 0
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    if [ "$#" -ne 1 ]; then
        echo "Usage: $0 <module>" >&2
        exit 2
    fi
    module_files "$1"
fi
