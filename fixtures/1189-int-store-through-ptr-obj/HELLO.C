int main(void) {
  int a = 1;
  int *p = &a;
  *p = 99;
  return a;
}
