int g;
int main(void) {
  enum { A = 1, B = 2, C = 3 } x;
  x = B;
  g = x;
  return 0;
}
