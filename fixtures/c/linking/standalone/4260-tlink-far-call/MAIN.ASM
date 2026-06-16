; Far CALL to an EXTRN FAR proc that lives in a different segment (FAR.OBJ).
; The segment part of the far pointer is load-relative, so TLINK emits a
; runtime relocation for it in the MZ header. Provenance: tasm MAIN.ASM.
        .MODEL  SMALL
        .STACK  100h
        EXTRN   FARPROC:FAR
        .CODE
START:  CALL    FARPROC                 ; 9A off:seg — seg word relocates
        MOV     AH, 4Ch
        INT     21h
        END     START
