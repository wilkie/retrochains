int main(void) {
  int x = 7;
  int far *p = (int far *)&x;
  return *p;
}
