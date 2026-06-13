int main(void) {
  int x = 5;
  int *p = (int *)&x;
  return *p;
}
