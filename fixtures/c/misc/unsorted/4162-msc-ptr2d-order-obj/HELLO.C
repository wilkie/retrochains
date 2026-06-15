int mat[3][3] = {{1,2,3},{4,5,6},{7,8,9}};
int f1(void) { int (*p)[3] = mat; return (*p)[1] + (*(p+1))[0]; }
int f2(void) { int (*p)[3] = mat; return (*(p+1))[0] + (*p)[1]; }
int f3(void) { int (*p)[3] = mat; return (*(p+1))[0] + (*(p+2))[1] + (*p)[2]; }
