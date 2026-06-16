; The canonical DOS startup: load DS from @DATA (the DGROUP paragraph).
; `MOV AX, @DATA` is a segment-base fixup (OMF location type 2) — the
; immediate is the group's load-relative paragraph, so TLINK emits a
; runtime relocation for it. Provenance: tasm MAIN.ASM -> MAIN.OBJ.
        .MODEL  SMALL
        .STACK  100h
        .DATA
val     DW      1234h
        .CODE
START:  MOV     AX, @DATA               ; seg-base imm -> relocation
        MOV     DS, AX
        MOV     AH, 4Ch
        INT     21h
        END     START
