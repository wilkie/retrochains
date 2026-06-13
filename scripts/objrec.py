#!/usr/bin/env python3
"""List OMF records (type, length, first bytes) of an OBJ."""
import sys
NAMES={0x80:'THEADR',0x88:'COMENT',0x8A:'MODEND',0x8C:'EXTDEF',0x90:'PUBDEF',
0x96:'LNAMES',0x98:'SEGDEF',0x9A:'GRPDEF',0x9C:'FIXUPP',0xA0:'LEDATA',
0xA2:'LIDATA',0xB0:'COMDEF',0x91:'PUBDEF',0x8E:'TYPDEF',0xB6:'COMDEF'}
def main():
    data=open(sys.argv[1],'rb').read()
    i=0
    while i<len(data):
        ty=data[i]; ln=data[i+1]|(data[i+2]<<8)
        payload=data[i+3:i+3+ln-1]
        nm=NAMES.get(ty,'?%02X'%ty)
        hexb=' '.join('%02x'%b for b in payload[:16])
        print(f"{i:04x} {nm:8} len={ln:3} {hexb}")
        i+=3+ln
main()
