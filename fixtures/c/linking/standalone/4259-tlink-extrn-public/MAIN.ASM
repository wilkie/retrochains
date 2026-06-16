; Caller module: references an EXTRN proc resolved from SUB.OBJ.
; Provenance: tasm MAIN.ASM -> MAIN.OBJ (tracked linker input).
        .MODEL  SMALL
        .STACK  100h
        EXTRN   ANSWER:PROC
        .CODE
START:  CALL    ANSWER                  ; AX <- 42, resolved across modules
        MOV     AH, 4Ch
        INT     21h
        END     START
