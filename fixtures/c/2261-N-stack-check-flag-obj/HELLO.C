int helper(int x) {
  int a[20];
  int i;
  for (i = 0; i < 20; i++) a[i] = i;
  return a[x];
}
int main(void) {
  return helper(10);
}
