int rowsum(int g[2][3])
{
  int total;
  int i;
  int j;

  total = 0;
  for (i = 0; i < 2; i++) {
    for (j = 0; j < 3; j++) {
      total += g[i][j];
    }
  }
  return total;
}

int main(void)
{
  static int m[2][3] = {{1, 2, 3}, {4, 5, 6}};
  return rowsum(m);
}
