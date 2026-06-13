#!/usr/bin/env python3
"""Extract _TEXT LEDATA bytes from an OMF OBJ and disassemble via ndisasm.

Usage: objdis.py <file.obj>
Assembles all LEDATA targeting the first SEGDEF named _TEXT (or seg index
given by --seg). Prints ndisasm 16-bit output.
"""
import sys, subprocess, struct

def records(data):
    i = 0
    while i < len(data):
        ty = data[i]
        ln = data[i+1] | (data[i+2] << 8)
        payload = data[i+3:i+3+ln-1]  # exclude checksum
        yield ty, payload
        i += 3 + ln

def main():
    path = sys.argv[1]
    data = open(path, 'rb').read()
    # find LNAMES to map seg names; find SEGDEF order
    lnames = ['']  # index 0 unused (1-based)
    seg_names = []  # seg_idx (1-based) -> class/seg name
    ledata = {}  # seg_idx -> bytes assembled by offset
    for ty, p in records(data):
        if ty == 0x96:  # LNAMES
            j = 0
            while j < len(p):
                l = p[j]; j += 1
                lnames.append(p[j:j+l].decode('latin1')); j += l
        elif ty == 0x98:  # SEGDEF16
            # ACBP, seg_len(2), seg_name_idx(1), class_idx(1), overlay(1)
            acbp = p[0]
            # if ACBP align field needs frame, skip (assume simple)
            off = 1
            seg_len = p[off] | (p[off+1]<<8); off += 2
            seg_name_idx = p[off]; off += 1
            seg_names.append(lnames[seg_name_idx] if seg_name_idx < len(lnames) else '?')
        elif ty == 0xa0:  # LEDATA16
            seg_idx = p[0]
            offset = p[1] | (p[2]<<8)
            d = p[3:]
            ledata.setdefault(seg_idx, bytearray())
            buf = ledata[seg_idx]
            if len(buf) < offset + len(d):
                buf.extend(b'\x00' * (offset + len(d) - len(buf)))
            buf[offset:offset+len(d)] = d
    # _TEXT seg index (1-based)
    text_idx = None
    for n, name in enumerate(seg_names, start=1):
        if name == '_TEXT':
            text_idx = n; break
    if text_idx is None:
        text_idx = 1
    code = bytes(ledata.get(text_idx, b''))
    if not code:
        print("(no _TEXT LEDATA)"); return
    p = subprocess.run(['ndisasm', '-b', '16', '/dev/stdin'], input=code,
                       capture_output=True)
    sys.stdout.write(p.stdout.decode('latin1'))

if __name__ == '__main__':
    main()
