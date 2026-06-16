; References ADDONE, which lives in a LIBRARY member (not named on the link
; line). TLINK must search MYLIB.LIB, find the member that defines ADDONE, and
; pull it in. Provenance: tasm MAIN.ASM -> MAIN.OBJ.
        .MODEL  SMALL
        .STACK  100h
        EXTRN   ADDONE:PROC
        .CODE
START:  MOV     AX, 5
        CALL    ADDONE                  ; resolved from MYLIB.LIB
        MOV     AH, 4Ch
        INT     21h
        END     START
