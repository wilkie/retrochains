int main(void) {
  int x;
  int r;
  x = 1;
  {
    int x;
    x = 100;
    r = x;
  }
  return r + x;
}
