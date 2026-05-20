int main(void) {
  int a[2][3];
  int sum;
  int i;
  int j;
  a[0][0] = 1;
  a[0][1] = 2;
  a[0][2] = 3;
  a[1][0] = 4;
  a[1][1] = 5;
  a[1][2] = 6;
  sum = 0;
  for (i = 0; i < 2; i++) {
    for (j = 0; j < 3; j++) {
      sum += a[i][j];
    }
  }
  return sum;
}
