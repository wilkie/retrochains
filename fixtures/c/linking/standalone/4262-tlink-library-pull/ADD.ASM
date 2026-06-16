; Library member: defines the PUBLIC symbol MAIN imports. Assembled to ADD.OBJ,
; then archived into MYLIB.LIB with `tlib MYLIB +ADD`.
        .MODEL  SMALL
        .CODE
        PUBLIC  ADDONE
ADDONE  PROC
        INC     AX
        RET
ADDONE  ENDP
        END
