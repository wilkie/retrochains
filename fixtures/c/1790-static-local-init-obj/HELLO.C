int incr(void) {
  static int n = 5;
  n++;
  return n;
}
int main(void) {
  incr();
  return incr();
}
