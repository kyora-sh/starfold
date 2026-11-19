#!/usr/bin/env bash
# linux-runtime-notices.sh <C compiler> <out dir>: the licence texts of the C runtime objects the
# compiler links into every Linux binary, from the packages that installed the objects it uses:
# glibc's startup files (Scrt1.o, crti.o: LGPL-2.1-or-later, whose startup files also grant
# unlimited permission to link them) and GCC's (crtbeginS.o: GPL-3.0-or-later with the GCC Runtime
# Library Exception). Writes glibc-copyright.txt, LGPL-2.1.txt, GCC-runtime-copyright.txt and
# GPL-3.0.txt; fails when an object's package or a text is missing.
set -euo pipefail
cc=${1:?usage: linux-runtime-notices.sh <C compiler> <out dir>}
out=$2
mkdir -p "$out"
fail() {
	echo "::error::linux-runtime-notices.sh: $*"
	exit 1
}
copyright() { # copyright <object> <file to write>: its package's copyright file
	local obj pkg doc base
	obj=$(realpath "$("$cc" -print-file-name="$1")")
	[ -f "$obj" ] || fail "$cc has no $1"
	pkg=$(dpkg -S "$obj" | head -n1 | cut -d: -f1)
	[ -n "$pkg" ] || fail "no package installed $obj"
	doc=/usr/share/doc/$pkg/copyright
	# GCC's library packages keep their copyright in the -base package they depend on.
	if [ ! -f "$doc" ]; then
		for base in $(dpkg-query -W -f '${Depends}' "$pkg" | tr ',|' '\n\n' | awk '{ print $1 }' | grep -E -- '-base(:[a-z0-9]+)?$'); do
			if [ -f "/usr/share/doc/${base%%:*}/copyright" ]; then
				doc=/usr/share/doc/${base%%:*}/copyright
				break
			fi
		done
	fi
	[ -f "$doc" ] || fail "neither $pkg (which installed $1) nor a -base package it depends on has a copyright file"
	echo "$1: $obj from $pkg, licence from $doc"
	cp "$doc" "$out/$2"
}
copyright Scrt1.o glibc-copyright.txt
grep -q -i 'Lesser General Public License' "$out/glibc-copyright.txt" || fail "glibc's copyright file doesn't name the LGPL"
copyright crtbeginS.o GCC-runtime-copyright.txt
grep -q 'GCC RUNTIME LIBRARY EXCEPTION' "$out/GCC-runtime-copyright.txt" || fail "GCC's copyright file lacks the runtime library exception"
grep -q 'Version 2.1, February 1999' /usr/share/common-licenses/LGPL-2.1 || fail "no LGPL-2.1 text"
grep -q 'Version 3, 29 June 2007' /usr/share/common-licenses/GPL-3 || fail "no GPL-3 text"
cp /usr/share/common-licenses/LGPL-2.1 "$out/LGPL-2.1.txt"
cp /usr/share/common-licenses/GPL-3 "$out/GPL-3.0.txt"
