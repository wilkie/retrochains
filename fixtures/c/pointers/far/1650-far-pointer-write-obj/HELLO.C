int main(void) {
  int x = 5;
  int far *p = (int far *)&x;
  *p = 99;
  return x;
}
