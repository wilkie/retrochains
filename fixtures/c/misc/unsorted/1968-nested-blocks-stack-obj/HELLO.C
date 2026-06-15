int main(void) {
  int outer = 100;
  {
    int inner = 50;
    outer += inner;
  }
  return outer;
}
