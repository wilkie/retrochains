int sum3(int a, int b, int c) {
  return a + b + c;
}
int main(void) {
  int i = 5;
  return sum3(i++, i++, i++);
}
