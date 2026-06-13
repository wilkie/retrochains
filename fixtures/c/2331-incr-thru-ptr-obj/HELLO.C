int main(void) {
  int x = 10;
  int *p = &x;
  (*p)++;
  ++*p;
  return *p;
}
