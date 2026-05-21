int main(void) {
  int x = 42;
  int far *p = (int far *)&x;
  return *p;
}
