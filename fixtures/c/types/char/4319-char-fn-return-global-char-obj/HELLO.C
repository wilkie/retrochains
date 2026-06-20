char g;
char first(void) {
  return g;
}
int main(void) {
  g = 'A';
  return (int)first();
}
