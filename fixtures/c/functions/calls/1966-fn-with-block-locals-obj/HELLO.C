int main(void) {
  int sum = 0;
  {
    int x = 10;
    sum += x;
  }
  {
    int y = 20;
    sum += y;
  }
  return sum;
}
