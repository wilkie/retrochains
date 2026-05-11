int f(int x) {
  int sum = 0;
  while (x > 0) {
    sum = sum + x;
    x = x - 1;
  }
  return sum;
}
int main(void) { return f(5); }
