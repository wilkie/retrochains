int main(void) {
  int x = 1;
  {
    int x = 2;
    {
      int x = 3;
      x = x + 10;
    }
  }
  return x;
}
