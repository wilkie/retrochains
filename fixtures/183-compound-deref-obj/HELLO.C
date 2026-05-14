int main(void) {
  int x = 10;
  int *p = &x;
  *p += 5;
  return x;
}
