#define SQUARE(x) ((x) * (x))
int main(void) {
  int i = 0;
  int r = SQUARE(++i);
  return r;
}
