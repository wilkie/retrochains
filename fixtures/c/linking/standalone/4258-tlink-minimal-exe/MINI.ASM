; Minimal standalone DOS program — no C runtime. Assembled with TASM into
; MINI.OBJ (the tracked linker input), then linked by TLINK into MINI.EXE.
; Provenance / regenerate the OBJ:
;     tasm MINI.ASM            (Borland TASM, under the bcc oracle)
; The committed MINI.OBJ is exactly those bytes; verify treats it as input.
        .MODEL  SMALL
        .STACK  100h
        .CODE
START:  MOV     AH, 4Ch                 ; DOS: terminate with return code
        MOV     AL, 0
        INT     21h
        END     START
