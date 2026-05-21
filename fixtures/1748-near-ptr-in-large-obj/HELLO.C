int main(void) {
  int x = 7;
  int near *p = (int near *)&x;
  return *p;
}
