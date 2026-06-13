char a[3] = {1, 2, 3};
int sumC(char *s, int n) {
  int t = 0;
  int i;
  for (i = 0; i < n; i++) t += s[i];
  return t;
}
int main(void) {
  return sumC(a, 3);
}
