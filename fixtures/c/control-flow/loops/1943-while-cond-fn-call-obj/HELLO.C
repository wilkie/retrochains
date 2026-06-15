int state = 0;
int read_inc(void) { return ++state; }
int main(void) {
  while (read_inc() < 5) ;
  return state;
}
