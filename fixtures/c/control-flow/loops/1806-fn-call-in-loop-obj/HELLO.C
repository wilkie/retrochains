int sqr(int x) { return x * x; }
int main(void) {
  int sum = 0;
  int i;
  for (i = 0; i < 4; i++) {
    sum += sqr(i);
  }
  return sum;
}
