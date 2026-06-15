int main(void)
{
  int a[2][2][2];
  int sum;
  int i;
  int j;
  int k;
  int n;

  n = 1;
  for (i = 0; i < 2; i++) {
    for (j = 0; j < 2; j++) {
      for (k = 0; k < 2; k++) {
        a[i][j][k] = n;
        n++;
      }
    }
  }
  sum = a[0][0][0] + a[1][1][1] + a[1][0][1];
  return sum;
}
