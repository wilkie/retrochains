int sum_grid(int g[2][3]) {
  return g[0][0] + g[1][2];
}
int main(void) {
  int m[2][3];
  m[0][0] = 7;
  m[0][1] = 0;
  m[0][2] = 0;
  m[1][0] = 0;
  m[1][1] = 0;
  m[1][2] = 13;
  return sum_grid(m);
}
