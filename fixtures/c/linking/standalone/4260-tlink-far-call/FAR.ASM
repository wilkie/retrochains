; Provider in its own PARA segment so the call above is genuinely far.
; Provenance: tasm FAR.ASM -> FAR.OBJ (tracked linker input).
FARSEG  SEGMENT PARA PUBLIC 'FARCODE'
        ASSUME  CS:FARSEG
        PUBLIC  FARPROC
FARPROC PROC    FAR
        MOV     AX, 1234h
        RET
FARPROC ENDP
FARSEG  ENDS
        END
