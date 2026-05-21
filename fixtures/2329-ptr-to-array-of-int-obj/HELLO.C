int main(void) {
  static int mat[3][3] = {{1,2,3},{4,5,6},{7,8,9}};
  int (*p)[3] = mat;
  return (*p)[1] + (*(p+1))[2] + (*(p+2))[0];
}
