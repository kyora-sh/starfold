# Usage: awk -f referrers.awk <objdump -R output> <objdump -d output> (symbols not demangled).
# Prints every function that refers to OpenH264's C entry points, directly or through other
# functions: by a call, a jump or an address taken in its code, either named in the instruction
# or through a GOT slot whose dynamic relocation points at the function. A reference held only in
# other data (a vtable, a table of pointers) is not seen.
function hex(x) {
    sub(/^0+/, "", x)
    return x == "" ? "0" : x
}
FNR == NR {
    if ($2 ~ /_RELATIVE$/ && $3 ~ /^\*ABS\*\+0x/) {
        a = $3
        sub(/^\*ABS\*\+0x/, "", a)
        slot[hex($1)] = "@" hex(a)
    } else if ($2 ~ /_GLOB_DAT$|_JUMP_SLOT$/) {
        s = $3
        sub(/@.*/, "", s)
        slot[hex($1)] = s
    }
    next
}
/^[0-9a-f]+ <[^<>]+>:$/ {
    cur = $2
    sub(/^</, "", cur)
    sub(/>:$/, "", cur)
    fnat[hex($1)] = cur
    next
}
cur != "" {
    line = $0
    if (match(line, /# [0-9a-f]+ </)) via[cur] = via[cur] " " hex(substr(line, RSTART + 2, RLENGTH - 4))
    while (match(line, /<[^<>]+>/)) {
        t = substr(line, RSTART + 1, RLENGTH - 2)
        line = substr(line, RSTART + RLENGTH)
        sub(/\+0x[0-9a-f]+$/, "", t)
        sub(/@plt$/, "", t)
        if (t != cur) by[t] = by[t] " " cur
    }
}
END {
    for (f in via) {
        m = split(via[f], as, " ")
        for (j = 1; j <= m; j++) {
            if (!(as[j] in slot)) continue
            t = slot[as[j]]
            if (substr(t, 1, 1) == "@") {
                t = substr(t, 2)
                if (!(t in fnat)) continue
                t = fnat[t]
            }
            if (t != f) by[t] = by[t] " " f
        }
    }
    n = split("WelsCreateSVCEncoder WelsDestroySVCEncoder WelsGetCodecVersion WelsGetCodecVersionEx", todo, " ")
    for (i = 1; i <= n; i++) seen[todo[i]] = 1
    while (n > 0) {
        t = todo[n--]
        m = split(by[t], from, " ")
        for (j = 1; j <= m; j++)
            if (!(from[j] in seen)) {
                seen[from[j]] = 1
                todo[++n] = from[j]
            }
    }
    for (s in seen) print s
}
