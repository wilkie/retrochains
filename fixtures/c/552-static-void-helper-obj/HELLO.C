static void set(int *p) {
  *p = 99;
}
int main(void) {
  int x;
  x = 0;
  set(&x);
  return x;
}
