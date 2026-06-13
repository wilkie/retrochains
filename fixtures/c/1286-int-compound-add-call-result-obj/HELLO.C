int twice(int x) {
  return x * 2;
}
int main(void) {
  int a = 5;
  a += twice(3);
  return a;
}
