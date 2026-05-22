int square(int x) { return x * x; }
int main(void) {
  int (*pfn)(int);
  pfn = square;
  return (*pfn)(7);
}
