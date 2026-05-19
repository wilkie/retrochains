int main(void) {
  int a = 5;
  int *p = &a;
  *p = *p + 1;
  return a;
}
