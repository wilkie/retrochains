int compute(int a, int b, int c, int d) {
  int sum = a + b + c + d;
  int prod = a * b;
  int diff = c - d;
  return sum + prod + diff;
}
int main(void) {
  return compute(1, 2, 3, 4);
}
