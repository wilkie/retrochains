int main(void)
{
  int m[3][3];
  int (*p)[3];
  int sum;
  int r;
  int c;

  for (r = 0; r < 3; r++) {
    for (c = 0; c < 3; c++) {
      m[r][c] = r * 3 + c;
    }
  }
  sum = 0;
  p = m;
  for (r = 0; r < 3; r++) {
    for (c = 0; c < 3; c++) {
      sum += (*p)[c];
    }
    p++;
  }
  return sum;
}
