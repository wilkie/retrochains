; Provider module: exports the PUBLIC proc MAIN.OBJ imports.
; Provenance: tasm SUB.ASM -> SUB.OBJ (tracked linker input).
        .MODEL  SMALL
        .CODE
        PUBLIC  ANSWER
ANSWER  PROC
        MOV     AX, 42
        RET
ANSWER  ENDP
        END
