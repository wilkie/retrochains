int main(void) {
  int sum = 0;
  {
    int a = 1;
    int b = 2;
    sum = a + b;
  }
  {
    int c = 10;
    sum += c;
  }
  return sum;
}
