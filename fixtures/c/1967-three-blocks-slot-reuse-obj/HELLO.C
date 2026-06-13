int main(void) {
  int sum = 0;
  { int a = 1; sum += a; }
  { int b = 2; sum += b; }
  { int c = 3; sum += c; }
  return sum;
}
