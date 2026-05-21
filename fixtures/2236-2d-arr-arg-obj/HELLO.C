int sum(int m[3][3]) {
  return m[0][0] + m[1][1] + m[2][2];
}
int main(void) {
  static int g[3][3] = {{1,2,3},{4,5,6},{7,8,9}};
  return sum(g);
}
