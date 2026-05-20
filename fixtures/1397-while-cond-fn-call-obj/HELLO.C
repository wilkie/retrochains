int x;
int next(void) {
  x++;
  return x;
}
int main(void) {
  while (next() < 3) ;
  return x;
}
